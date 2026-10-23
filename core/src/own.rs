//! The files ral's programs touch on their own account, outside any shell.
//!
//! Three kinds: what a program keeps for itself ([`OwnDir`]), what the user
//! writes at the places it chooses ([`UserFile`]), and what the user names to
//! it ([`read_named`], [`open_named`], [`kind_named`], [`create_named`]).
//!
//! No grant judges any of them, and none raises a card: the authority is the
//! user's own, minted by [`Ungoverned`], and no model named the object.  They
//! differ in the address.  Beneath an [`OwnDir`] each name is one entry the
//! program chose and no link is followed, since the program made none.  A
//! [`UserFile`] and a user's name are walked with their links followed, since
//! the user's links are how the user keeps them.
//!
//! Nothing here is made from an arbitrary path; the places are the program's
//! layout, and the one write a user's name admits is a new file.
//!
//! This module contains no `site!`: every syscall it needs is a primitive of
//! [`crate::path`]'s walk.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde::de::DeserializeOwned;
use strum::IntoStaticStr;

use crate::capability::Ungoverned;
use crate::host;
use crate::path::walk::{Access, dangling, walk_dir_made};
use crate::path::{
    Dir, EntryName, Judged, Kind, LeafMode, LexicalPath, Located, Reading, Resolver, Stat,
    Visibility, Writing, XdgKind, app_dir, shown, spelled_parent, walk, walk_dir,
};
use crate::sync::LockExt as _;
use crate::types::EnvVars;

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;

/// How many times a name is made over, where another process makes it first.
const MAKE_TRIES: usize = 3;

/// The roots resolved so far, by the spelling of the base each was resolved
/// for: [`OwnDir::xdg`] walks a base once and hands the handle out after.
static ROOTS: Mutex<BTreeMap<PathBuf, OwnDir>> = Mutex::new(BTreeMap::new());

// ── The minting points ────────────────────────────────────────────────

/// `l`, vouched for as the user's own, for reading.
fn reading(l: Located) -> Located<Judged<Reading>> {
    Ungoverned::read(l)
}

/// `l`, vouched for as the user's own, for writing.
fn writing(l: Located) -> Located<Judged<Writing>> {
    Ungoverned::write(l)
}

/// `d`, vouched for as the user's own, for writing.
fn writing_dir(d: Dir) -> Dir<Judged<Writing>> {
    Ungoverned::write_dir(d)
}

// ── Errors ────────────────────────────────────────────────────────────

/// What an operation was doing when the OS refused it, as an error says it.
#[derive(Clone, Copy, Debug, IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum Act {
    Open,
    Make,
    Read,
    Write,
    Rename,
    Remove,
    List,
}

/// Why an operation failed: one sentence naming the place by
/// [`shown`] and, where the user can act, what to do.
#[derive(Debug)]
pub enum OwnError {
    /// No base for `kind`: no absolute `XDG_<KIND>_HOME`, no home bound, and
    /// none in the user database.
    NoBase(XdgKind),
    /// A relative name, in a process with no cwd to read it against.
    NoCwd(PathBuf),
    /// A link where the program keeps a file of its own.
    Linked(PathBuf),
    /// `at`, a file the user writes or names or a base of the program's
    /// layout, is a link to `to`, which is not there.
    Dangling { at: PathBuf, to: PathBuf },
    /// `at` is not beneath `root`.
    Outside { at: PathBuf, root: PathBuf },
    /// The OS refused `act` at `at`; `to` is where a link at `at` led.
    Io {
        act: Act,
        at: PathBuf,
        to: Option<PathBuf>,
        error: io::Error,
    },
    /// The JSON at `at` is not the value `act` was to read, or the value
    /// `act` was to write has no JSON.
    Json {
        act: Act,
        at: PathBuf,
        error: serde_json::Error,
    },
}

impl OwnError {
    /// The OS's kind; `NotFound` for a base, a cwd or a target there is none
    /// of; `InvalidInput` for a link or a place outside; `InvalidData` for
    /// JSON that is not the value's.
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::NoBase(_) | Self::NoCwd(_) | Self::Dangling { .. } => io::ErrorKind::NotFound,
            Self::Linked(_) | Self::Outside { .. } => io::ErrorKind::InvalidInput,
            Self::Io { error, .. } => error.kind(),
            Self::Json { .. } => io::ErrorKind::InvalidData,
        }
    }

    /// Whether nothing was at the place: the OS said `NotFound`.
    fn is_absent(&self) -> bool {
        matches!(self, Self::Io { error, .. } if error.kind() == io::ErrorKind::NotFound)
    }
}

impl fmt::Display for OwnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBase(kind) => {
                let (name, var) = (<&str>::from(*kind), kind.env_var());
                write!(
                    f,
                    "no {name} directory: {var} is not an absolute path, and this process has no \
                     home directory; set HOME (or {var}) and run it again"
                )
            }
            Self::NoCwd(at) => write!(
                f,
                "{} is a relative path, and this process has no working directory to read it \
                 against: give an absolute path",
                shown(at)
            ),
            Self::Linked(at) => write!(
                f,
                "{} is a link, and a program's own files are written through no link: remove the \
                 link, and it will be made again",
                shown(at)
            ),
            Self::Dangling { at, to } => write!(
                f,
                "{} is a link to {}, which is not there: restore it (is the dotfiles checkout \
                 present?) or remove the link",
                shown(at),
                shown(to)
            ),
            Self::Outside { at, root } => write!(
                f,
                "{} is not under {}, where this program keeps its own: move it there to use it",
                shown(at),
                shown(root)
            ),
            Self::Io {
                act: Act::Make,
                at,
                error,
                ..
            } if error.kind() == io::ErrorKind::AlreadyExists => write!(
                f,
                "{} already exists, and is never overwritten: choose another name",
                shown(at)
            ),
            Self::Io {
                act: Act::Write,
                at,
                to: Some(to),
                error,
            } if matches!(
                error.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
            ) =>
            {
                write!(
                    f,
                    "could not write {}: it is a link to {}, which cannot be changed ({error}); \
                     change it where it is generated (home-manager, for one), or replace the \
                     link with a file",
                    shown(at),
                    shown(to)
                )
            }
            Self::Io { act, at, error, .. } => {
                write!(f, "could not {} {}: {error}", <&str>::from(*act), shown(at))
            }
            Self::Json { act, at, error } => write!(
                f,
                "could not {} {} as JSON: {error}",
                <&str>::from(*act),
                shown(at)
            ),
        }
    }
}

impl std::error::Error for OwnError {}

impl From<OwnError> for io::Error {
    /// The kind kept, the sentence the payload.
    fn from(e: OwnError) -> Self {
        Self::new(e.kind(), e.to_string())
    }
}

impl From<OwnError> for String {
    /// The sentence: what `?` makes in a `Result<_, String>`.
    fn from(e: OwnError) -> Self {
        e.to_string()
    }
}

/// The OS's refusal of `act` at `at`.
fn io_at(act: Act, at: &Path, error: io::Error) -> OwnError {
    OwnError::Io {
        act,
        at: at.to_path_buf(),
        to: None,
        error,
    }
}

/// A walk's refusal `error` of `act` at `at`, said as what to do: a link at
/// `at` that leads nowhere is [`OwnError::Dangling`].
fn walked(act: Act, at: &Path, error: io::Error) -> OwnError {
    if let Some(to) = dangling(&error) {
        return OwnError::Dangling {
            at: at.to_path_buf(),
            to: to.to_path_buf(),
        };
    }
    io_at(act, at, error)
}

/// [`OwnError::Dangling`] where `found`, the walk of `at`, ended absent past a
/// link: the link's target is not there.
fn dangled(at: &Path, found: &Located) -> Option<OwnError> {
    let to = found
        .linked_to()
        .filter(|_| found.trace().kind().is_none())?;
    Some(OwnError::Dangling {
        at: at.to_path_buf(),
        to: to.to_path_buf(),
    })
}

// ── Roots and spellings ───────────────────────────────────────────────

/// `path` as a spelling to walk: an absolute one as it stands, a relative one
/// read against the process's cwd.
fn anchored(path: &Path) -> Result<LexicalPath, OwnError> {
    if let Some(absolute) = LexicalPath::absolute(path) {
        return Ok(absolute);
    }
    let cwd = host::cwd().ok_or_else(|| OwnError::NoCwd(path.to_path_buf()))?;
    Ok(Resolver::at(&EnvVars::new(), &cwd).anchor(path))
}

/// The directory `spelling` names, held, each absent name of it made
/// owner-only first; `at` is how errors name it.
fn made(spelling: &LexicalPath, at: &Path) -> Result<Dir<Judged<Writing>>, OwnError> {
    walk_dir_made(spelling)
        .map(writing_dir)
        .map_err(|e| walked(Act::Make, at, e))
}

/// A directory of the program's own, resolved from `path` afresh: not
/// remembered.  It is spelled as it was walked, which makes a relative `path`
/// absolute.
fn root(path: &Path) -> Result<OwnDir, OwnError> {
    let spelling = anchored(path)?;
    let dir = made(&spelling, path)?;
    Ok(OwnDir::new(dir, spelling))
}

// ── OwnDir ────────────────────────────────────────────────────────────

/// A directory the program keeps for itself, held.
///
/// Every operation is relative to its handle and names one entry, and no link
/// is followed at a name beneath it.  Shows as its path.  `Clone` shares the
/// handle.
#[derive(Clone)]
pub struct OwnDir(Arc<Held>);

/// The handle of an [`OwnDir`] and the spelling it was reached by.
struct Held {
    dir: Dir<Judged<Writing>>,
    path: LexicalPath,
}

/// One entry of a listing, with what a stat of it found, not followed.
#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub stat: Stat,
}

impl OwnDir {
    fn new(dir: Dir<Judged<Writing>>, path: LexicalPath) -> Self {
        Self(Arc::new(Held { dir, path }))
    }

    /// `app`'s directory under the host's `kind` base, `$XDG_<kind>_HOME/<app>/`
    /// ([`app_dir`]): walked from the root with links
    /// followed, each absent component made owner-only, as the XDG spec asks.
    /// Resolved once per process for each base it spells: a second call is the
    /// first's handle, and a host whose base has changed resolves a new root.
    ///
    /// # Errors
    /// [`OwnError::NoBase`] where the host names no `kind` base and the user
    /// database no home; [`OwnError::Dangling`] where a link on the way leads
    /// nowhere; the walk's, or a make's.
    pub fn xdg(app: &'static str, kind: XdgKind) -> Result<Self, OwnError> {
        let path = app_dir(kind, app).ok_or(OwnError::NoBase(kind))?;
        let known = ROOTS.lock_ignore_poison().get(&path).cloned();
        if let Some(resolved) = known {
            return Ok(resolved);
        }
        // Walked outside the lock.  Where another call walked the same base
        // meanwhile, its handle was stored first, and is the one handed out.
        let resolved = root(&path)?;
        Ok(ROOTS
            .lock_ignore_poison()
            .entry(path)
            .or_insert(resolved)
            .clone())
    }

    /// Where it is, for a consumer that takes names: a child's argv or
    /// environment, a grant, a crate with sites of its own.  The spelling it
    /// was reached by; the one place a name leaves the handle, and what the
    /// consumer does with it is resolved again, by the consumer.
    pub fn path(&self) -> &Path {
        self.0.path.as_path()
    }

    /// The directory beneath this one that `path` names, reached again from
    /// this handle by its entry names: how a user points at one of the
    /// program's own directories.  `path` is walked with its links followed,
    /// and compared by where it lands, not by its spelling, so a spelling
    /// through a link above this directory, or in another case on Windows,
    /// reaches it too.  A relative `path` is read against the process's cwd,
    /// and this directory itself is beneath itself.
    ///
    /// # Errors
    /// [`OwnError::Outside`] where it lands elsewhere; [`OwnError::NoCwd`];
    /// the walk's; and [`dir`](Self::dir)'s for each name down.
    pub fn within(&self, path: &Path) -> Result<Self, OwnError> {
        let outside = || OwnError::Outside {
            at: path.to_path_buf(),
            root: self.path().to_path_buf(),
        };
        let at = walk_dir(&anchored(path)?).map_err(|e| walked(Act::Open, path, e))?;
        let names = (at.real().relative_to(self.0.dir.real())).ok_or_else(outside)?;
        names
            .iter()
            .try_fold(self.clone(), |dir, name| dir.descend(name))
    }

    /// The directory `name`.
    ///
    /// # Errors
    /// `NotFound`; [`OwnError::Linked`]; `NotADirectory`.
    pub fn dir(&self, name: &str) -> Result<Self, OwnError> {
        self.descend(&self.entry(Act::Open, name)?)
    }

    /// The directory `name`, made owner-only where absent.
    ///
    /// # Errors
    /// [`OwnError::Linked`]; `NotADirectory`; the make's.
    pub fn make_dir(&self, name: &str) -> Result<Self, OwnError> {
        self.make_entry(&self.entry(Act::Make, name)?)
    }

    /// The entries, in name order.  A name that is not Unicode is none of
    /// the program's, and an entry gone before its stat is gone: both skipped.
    ///
    /// # Errors
    /// The listing's, or the stat of an entry that is there and cannot be
    /// stated.
    pub fn list(&self) -> Result<Vec<Entry>, OwnError> {
        let listing = (self.0.dir.read_dir()).map_err(|e| io_at(Act::List, self.path(), e))?;
        let mut entries = Vec::new();
        for listed in listing {
            let Ok(name) = listed.name().as_os_str().to_owned().into_string() else {
                continue;
            };
            match listed.seen() {
                Ok(seen) => entries.push(Entry {
                    name,
                    stat: seen.stat.clone(),
                }),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    let error = io::Error::new(e.kind(), e.to_string());
                    return Err(io_at(Act::List, &self.at(listed.name()), error));
                }
            }
        }
        Ok(entries)
    }

    /// What is at `name`, not followed; `None` where nothing is, or nothing
    /// can be said.
    pub fn stat(&self, name: &str) -> Option<Stat> {
        let name = self.entry(Act::Open, name).ok()?;
        self.find(Act::Open, &name).ok()?.stat().cloned()
    }

    /// The JSON value in `name`; `Ok(None)` where nothing is at `name`.  A
    /// caller that reads a cache or a remembered choice, and wants a corrupt
    /// file to read as none, writes `.ok().flatten()` and says so.
    ///
    /// # Errors
    /// [`OwnError::Linked`] for a link; the read's; [`OwnError::Json`] for
    /// text that is not the value.
    pub fn load<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, OwnError> {
        let bytes = match self.read(name) {
            Ok(bytes) => bytes,
            Err(e) if e.is_absent() => return Ok(None),
            Err(e) => return Err(e),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| OwnError::Json {
                act: Act::Read,
                at: self.path().join(name),
                error,
            })
    }

    /// Replace `name` with `value` as JSON, whole: staged beside it, synced,
    /// renamed over it, the directory synced, so a reader sees the old file or
    /// the new.  `Public` keeps a present file's mode; `Secret` is owner-only
    /// whatever was there.
    ///
    /// # Errors
    /// [`OwnError::Linked`] for a link; [`OwnError::Json`] for a value with no
    /// JSON; the write's.
    pub fn store<T: Serialize>(
        &self,
        name: &str,
        value: &T,
        visibility: Visibility,
    ) -> Result<(), OwnError> {
        let name = self.entry(Act::Write, name)?;
        let json = serde_json::to_vec_pretty(value).map_err(|error| OwnError::Json {
            act: Act::Write,
            at: self.at(&name),
            error,
        })?;
        let failed = |e| self.failed(Act::Write, &name, e);
        let mut staged = self
            .unlinked(Act::Write, &name)?
            .stage(visibility)
            .map_err(failed)?;
        staged.file().write_all(&json).map_err(failed)?;
        staged.publish().map_err(failed)
    }

    /// The bytes of `name`.
    ///
    /// # Errors
    /// `NotFound`; [`OwnError::Linked`]; the read's.
    pub fn read(&self, name: &str) -> Result<Vec<u8>, OwnError> {
        let name = self.entry(Act::Read, name)?;
        let mut bytes = Vec::new();
        (self.opened(Act::Read, &name)?)
            .read_to_end(&mut bytes)
            .map_err(|e| self.failed(Act::Read, &name, e))?;
        Ok(bytes)
    }

    /// A read handle on `name`: for a reader that seeks, and for a lock taken
    /// only on a file already there ([`File::try_lock`]).
    ///
    /// # Errors
    /// `NotFound`; [`OwnError::Linked`]; the open's.
    pub fn open(&self, name: &str) -> Result<File, OwnError> {
        self.opened(Act::Open, &self.entry(Act::Open, name)?)
    }

    /// `name` for appending, made where absent: each write lands whole at the
    /// end, whoever else appends.
    ///
    /// # Errors
    /// [`OwnError::Linked`]; the open's.
    pub fn append(&self, name: &str) -> Result<File, OwnError> {
        self.open_made(name, Access::Append)
    }

    /// `name` for writing in place, made where absent and never truncated: a
    /// lock file to hold ([`File::lock`]), a log to cut back ([`File::set_len`]).
    ///
    /// # Errors
    /// [`OwnError::Linked`]; the open's.
    pub fn open_write(&self, name: &str) -> Result<File, OwnError> {
        self.open_made(name, Access::Write)
    }

    /// A file made now for writing under an invented name: `prefix`, sixteen
    /// random alphanumerics, `suffix`.  The name and the handle.
    ///
    /// # Errors
    /// The make's; `InvalidInput` for a `prefix` or `suffix` that is no part
    /// of an entry name.
    pub fn create_fresh(
        &self,
        prefix: &str,
        suffix: &str,
        visibility: Visibility,
    ) -> Result<(String, File), OwnError> {
        let text = EntryName::fresh_text(prefix, suffix);
        let name = EntryName::parse(OsStr::new(&text))
            .map_err(|e| io_at(Act::Make, &self.path().join(prefix), e.into()))?;
        let file = writing(self.0.dir.absent(name.clone()))
            .open_data(Access::Write, visibility)
            .map_err(|e| self.failed(Act::Make, &name, e))?;
        Ok((text, file))
    }

    /// Rename `from` to `to` in this directory, over whatever `to` is.
    ///
    /// # Errors
    /// `NotFound` where `from` is not there; the rename's.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), OwnError> {
        let (from, to) = (self.entry(Act::Rename, from)?, self.entry(Act::Rename, to)?);
        (self.find(Act::Rename, &from)?)
            .rename(&to)
            .map_err(|e| self.failed(Act::Rename, &from, e))
    }

    /// Remove `name`, whatever it is: a directory with everything beneath it,
    /// a link as the link.  Nothing is followed; nothing there is no error.
    ///
    /// # Errors
    /// The removal's.
    pub fn remove(&self, name: &str) -> Result<(), OwnError> {
        self.remove_entry(&self.entry(Act::Remove, name)?)
    }

    /// The host's temp directory itself, held, for a test double that names a
    /// directory and makes nothing in it.  Each name of it that is not there
    /// is made owner-only, as for [`xdg`](Self::xdg); nothing else is made and
    /// nothing is removed.  A double that makes something holds a
    /// [`TestDir`] ([`for_test`](Self::for_test)).
    ///
    /// # Errors
    /// The temp directory's.
    #[cfg(any(test, feature = "test-util"))]
    pub fn temp_for_test() -> Result<Self, OwnError> {
        root(&host::temp_dir())
    }

    /// A fresh directory of its own in the temp directory, for a test double
    /// that makes something in it; removed when the guard is dropped.
    ///
    /// # Errors
    /// The temp directory's, or the make's.
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(tag: &str) -> Result<TestDir, OwnError> {
        let parent = Self::temp_for_test()?;
        let name = EntryName::fresh(&format!("ral-test-{tag}-"))
            .map_err(|e| io_at(Act::Make, &parent.path().join(tag), e.into()))?;
        let dir = parent.make_entry(&name)?;
        Ok(TestDir {
            dir,
            _removal: Removal { parent, name },
        })
    }

    // ── Operations on one entry ──

    /// The spelling of the entry `name`.
    fn at(&self, name: &EntryName) -> PathBuf {
        self.path().join(name.as_os_str())
    }

    /// `name` as an entry name; an error of `act` where it is none.
    fn entry(&self, act: Act, name: &str) -> Result<EntryName, OwnError> {
        EntryName::parse(OsStr::new(name))
            .map_err(|e| io_at(act, &self.path().join(name), e.into()))
    }

    /// The OS's refusal of `act` at the entry `name`.
    fn failed(&self, act: Act, name: &EntryName, error: io::Error) -> OwnError {
        io_at(act, &self.at(name), error)
    }

    /// The directory below, one step down, as [`dir`](Self::dir) reaches it.
    fn descend(&self, name: &EntryName) -> Result<Self, OwnError> {
        match self.0.dir.open_dir(name) {
            Ok(dir) => Ok(Self::new(writing_dir(dir), self.0.path.join(name))),
            // The step refuses a link and any other non-directory alike.
            Err(e) if e.kind() == io::ErrorKind::NotADirectory && self.is_link(name) => {
                Err(OwnError::Linked(self.at(name)))
            }
            Err(e) => Err(self.failed(Act::Open, name, e)),
        }
    }

    /// [`descend`](Self::descend), the directory made where absent.
    fn make_entry(&self, name: &EntryName) -> Result<Self, OwnError> {
        match self.descend(name) {
            Err(OwnError::Io { error, .. }) if error.kind() == io::ErrorKind::NotFound => {}
            found => return found,
        }
        match writing(self.0.dir.absent(name.clone())).create_dir(Visibility::Private) {
            Ok(dir) => Ok(Self::new(writing_dir(dir), self.0.path.join(name))),
            // Another process made it first: it is there now.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self.descend(name),
            Err(e) => Err(self.failed(Act::Make, name, e)),
        }
    }

    /// The entry `name` as the walk finds it, nothing followed: the object it
    /// is, or the create target it would be.
    fn find(&self, act: Act, name: &EntryName) -> Result<Located<Judged<Writing>>, OwnError> {
        (self.0.dir.target(name, LeafMode::NoFollow))
            .map(writing)
            .map_err(|e| self.failed(act, name, e))
    }

    /// Whether a link is at `name`.
    fn is_link(&self, name: &EntryName) -> bool {
        self.find(Act::Open, name)
            .is_ok_and(|found| found.kind() == Some(Kind::Symlink))
    }

    /// [`find`](Self::find), refusing a link: where the program keeps a file
    /// of its own, it is written through none.
    fn unlinked(&self, act: Act, name: &EntryName) -> Result<Located<Judged<Writing>>, OwnError> {
        let found = self.find(act, name)?;
        if found.kind() == Some(Kind::Symlink) {
            return Err(OwnError::Linked(self.at(name)));
        }
        Ok(found)
    }

    /// A read handle on the entry `name`.
    fn opened(&self, act: Act, name: &EntryName) -> Result<File, OwnError> {
        (self.unlinked(act, name)?)
            .read()
            .map_err(|e| self.failed(act, name, e))
    }

    /// The entry `name` opened for `access`, made where absent.
    fn open_made(&self, name: &str, access: Access) -> Result<File, OwnError> {
        let name = self.entry(Act::Open, name)?;
        let mut tries = 1;
        loop {
            let opened = (self.unlinked(Act::Open, &name)?).open_data(access, Visibility::Public);
            match opened {
                // Another process made it between the walk and the make.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tries < MAKE_TRIES => {
                    tries += 1;
                }
                opened => return opened.map_err(|e| self.failed(Act::Open, &name, e)),
            }
        }
    }

    /// [`remove`](Self::remove), for an entry name.
    fn remove_entry(&self, name: &EntryName) -> Result<(), OwnError> {
        match self.find(Act::Remove, name)?.remove() {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            removed => removed.map_err(|e| self.failed(Act::Remove, name, e)),
        }
    }
}

impl fmt::Display for OwnDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&shown(self.path()))
    }
}

impl fmt::Debug for OwnDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OwnDir").field(&shown(self.path())).finish()
    }
}

/// A directory made by [`OwnDir::for_test`], removed when dropped.  It
/// dereferences to the directory.
#[cfg(any(test, feature = "test-util"))]
pub struct TestDir {
    dir: OwnDir,
    // Declared after `dir`, so it runs once the handle is let go: Windows
    // will not remove a directory something holds.
    _removal: Removal,
}

#[cfg(any(test, feature = "test-util"))]
impl std::ops::Deref for TestDir {
    type Target = OwnDir;

    fn deref(&self) -> &OwnDir {
        &self.dir
    }
}

/// The removal of a test directory, from the directory it is in.
#[cfg(any(test, feature = "test-util"))]
struct Removal {
    parent: OwnDir,
    name: EntryName,
}

#[cfg(any(test, feature = "test-util"))]
impl Drop for Removal {
    fn drop(&mut self) {
        let _ = self.parent.remove_entry(&self.name);
    }
}

// ── UserFile ──────────────────────────────────────────────────────────

/// A file the user writes, at a place the program chooses: an rc, a profile,
/// a provider config, house rules.
///
/// Spelled from the program's layout alone and resolved afresh by each
/// operation with links followed, since a dotfile manager's link is how the
/// user keeps it: a replace lands beside the file the link reaches, so the
/// link survives and the repository behind it keeps the change.
#[derive(Clone, Debug)]
pub struct UserFile {
    path: PathBuf,
}

impl UserFile {
    /// `name` in `app`'s directory under the host's `kind` base.
    ///
    /// # Errors
    /// [`OwnError::NoBase`].
    pub fn xdg(app: &'static str, kind: XdgKind, name: &'static str) -> Result<Self, OwnError> {
        let dir = app_dir(kind, app).ok_or(OwnError::NoBase(kind))?;
        Ok(Self {
            path: dir.join(name),
        })
    }

    /// `~/<name>`, the single-file form older loaders read; `None` with no home.
    pub fn home(name: &'static str) -> Option<Self> {
        host::home_dot(name).map(|path| Self { path })
    }

    /// `<dir>/<name>`, a fixed place an administrator writes for every user.
    pub fn fixed(dir: &'static str, name: &'static str) -> Self {
        Self {
            path: PathBuf::from(dir).join(name),
        }
    }

    /// Where it is spelled, for a message or a source's origin.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Its text, links followed; `None` where nothing is.
    ///
    /// # Errors
    /// [`OwnError::Dangling`] for a link to nothing; `InvalidData` for text
    /// that is not UTF-8; the read's.
    pub fn read(&self) -> Result<Option<String>, OwnError> {
        let outcome = walk(&anchored(&self.path)?, LeafMode::Follow);
        let Some(found) = self.reached(outcome)? else {
            return Ok(None);
        };
        let failed = |e| io_at(Act::Read, &self.path, e);
        let mut text = String::new();
        (reading(found).read().map_err(failed)?)
            .read_to_string(&mut text)
            .map_err(failed)?;
        Ok(Some(text))
    }

    /// Replace it with `text` whole, through any link: staged beside the file
    /// the link reaches, renamed over it, its mode kept.  The directory is
    /// made where absent.
    ///
    /// # Errors
    /// Where the file the link reaches cannot be replaced, an error naming it
    /// and saying where to change it instead; [`OwnError::Dangling`] for a
    /// link to a name beneath a directory that is not there; the make's and
    /// the write's.
    pub fn replace(&self, text: &str) -> Result<(), OwnError> {
        let (dir, name) = self.placed()?;
        let target = (dir.target(&name, LeafMode::Follow))
            .map(writing)
            .map_err(|e| walked(Act::Write, &self.path, e))?;
        let to = target.linked_to().map(Path::to_path_buf);
        let failed = |error| OwnError::Io {
            act: Act::Write,
            at: self.path.clone(),
            to: to.clone(),
            error,
        };
        let mut staged = target.stage(Visibility::Public).map_err(failed)?;
        staged.file().write_all(text.as_bytes()).map_err(failed)?;
        staged.publish().map_err(failed)
    }

    /// Write `text` only where nothing at all is at the name; `false` where
    /// something is, a link included, dangling or not: what stands there is
    /// the user's.  The directory is made where absent.
    ///
    /// # Errors
    /// The make's and the write's.
    pub fn seed(&self, text: &str) -> Result<bool, OwnError> {
        let (dir, name) = self.placed()?;
        let found = (dir.target(&name, LeafMode::NoFollow))
            .map(writing)
            .map_err(|e| walked(Act::Make, &self.path, e))?;
        let mut file = match found.create_new(Access::Write, Visibility::Public) {
            Ok(file) => file,
            // Something is there, or another process seeded it between the
            // walk and the make.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(io_at(Act::Make, &self.path, e)),
        };
        let written = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all());
        if let Err(e) = written {
            drop(file);
            // A skeleton cut short must not stand as the user's own.
            let _ = (dir.locate(&name, LeafMode::NoFollow))
                .map(writing)
                .and_then(Located::remove);
            return Err(io_at(Act::Write, &self.path, e));
        }
        Ok(true)
    }

    /// The directory the file sits in, made where absent and held, and the
    /// file's name in it.
    fn placed(&self) -> Result<(Dir<Judged<Writing>>, EntryName), OwnError> {
        let spelling = anchored(&self.path)?;
        let (Some(dir), Some(name)) = (
            spelled_parent(spelling.as_path()).and_then(LexicalPath::absolute),
            spelling.as_path().file_name(),
        ) else {
            let error = io::Error::new(io::ErrorKind::InvalidInput, "it names no file");
            return Err(io_at(Act::Open, &self.path, error));
        };
        let name = EntryName::parse(name).map_err(|e| io_at(Act::Open, &self.path, e.into()))?;
        Ok((made(&dir, dir.as_path())?, name))
    }

    /// What a walk of the file reached through its links: `None` where
    /// nothing is there.
    fn reached(&self, outcome: io::Result<Located>) -> Result<Option<Located>, OwnError> {
        match outcome {
            Ok(found) => match dangled(&self.path, &found) {
                Some(dangling) => Err(dangling),
                None => Ok(found.trace().kind().is_some().then_some(found)),
            },
            Err(e) => match walked(Act::Read, &self.path, e) {
                absent if absent.is_absent() => Ok(None),
                refused => Err(refused),
            },
        }
    }
}

// ── The files a user names ────────────────────────────────────────────

// `clippy.toml` bans these four, so that each caller is a typed site saying
// whose name it is.  None calls another: they share the helpers above and
// `opened_named`.

/// A read handle on the file the user named, links followed.
fn opened_named(path: &Path) -> Result<File, OwnError> {
    let found = walk(&anchored(path)?, LeafMode::Follow).map_err(|e| walked(Act::Open, path, e))?;
    if let Some(dangling) = dangled(path, &found) {
        return Err(dangling);
    }
    reading(found).read().map_err(|e| io_at(Act::Open, path, e))
}

/// The text of the file the user named, links followed; a relative `path` is
/// read against the process's cwd.
///
/// # Errors
/// [`OwnError::NoCwd`]; `NotFound`; [`OwnError::Dangling`] for a link to
/// nothing; `InvalidData` for text that is not UTF-8; the read's.
pub fn read_named(path: &Path) -> Result<String, OwnError> {
    let mut text = String::new();
    opened_named(path)?
        .read_to_string(&mut text)
        .map_err(|e| io_at(Act::Read, path, e))?;
    Ok(text)
}

/// A read handle on the file the user named, links followed.
///
/// # Errors
/// [`OwnError::NoCwd`]; `NotFound`; [`OwnError::Dangling`] for a link to
/// nothing; the open's.
pub fn open_named(path: &Path) -> Result<File, OwnError> {
    opened_named(path)
}

/// What the user's name reaches, links followed; `None` where nothing is, a
/// link to nothing included.
pub fn kind_named(path: &Path) -> Option<Kind> {
    walk(&anchored(path).ok()?, LeafMode::Follow)
        .ok()?
        .trace()
        .kind()
}

/// The file the user named, made now for writing: never over anything there,
/// a link included.  The one write a user's name admits.
///
/// # Errors
/// [`OwnError::NoCwd`]; `AlreadyExists` where anything is at the name, which
/// reads as the sentence saying so; the make's.
pub fn create_named(path: &Path) -> Result<File, OwnError> {
    let found =
        walk(&anchored(path)?, LeafMode::NoFollow).map_err(|e| walked(Act::Make, path, e))?;
    writing(found)
        .create_new(Access::Write, Visibility::Public)
        .map_err(|e| io_at(Act::Make, path, e))
}
