//! A path's names, read by spelling alone: its last, its parent, its
//! extension, its names beneath a root, and its text.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use super::head::is_sep_byte;
use crate::path::site::Silent;
use crate::path::{EntryName, LexicalPath, NotAnEntryName, PathRules};
use crate::site::site;

/// Whether `path` is spelled as a directory: it ends in a separator, or in
/// one and `.`, which `Path::components` does not show.
pub(crate) fn names_a_dir(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    let bytes = bytes.strip_suffix(b".").unwrap_or(bytes);
    bytes
        .last()
        .is_some_and(|&b| is_sep_byte(PathRules::HOST, b))
}

/// `path.parent()`, or `.` when that is absent or empty.
///
/// Callers feeding the result to a `*_in(parent)` API
/// (`tempfile::Builder::tempfile_in`, opening the directory to fsync it)
/// therefore don't choke on a bare filename.
pub(crate) fn parent_or_cwd(path: &Path) -> &Path {
    site!(Silent::ParentOrCwd, path => parent())
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| site!(Silent::ParentOrCwd, Path::new(".")))
}

/// `path` with its last name dropped, by spelling alone.
///
/// No link is seen through and no `..` folded, so it is the parent only of a
/// path whose names are the ones on disk.  For a program's own layout (the
/// folder a file it writes sits in, its install tree, a run folder it made),
/// never for a name a grant judges.
#[must_use]
pub fn spelled_parent(path: &Path) -> Option<&Path> {
    site!(Silent::SpelledParent, path => parent())
}

/// `path`'s last name, by spelling alone: `None` for a root or a path
/// ending in `..`.
#[must_use]
pub fn file_name(path: &Path) -> Option<&OsStr> {
    site!(Silent::SpelledName, path => file_name())
}

/// `shown` relative to `root`, its names slash-joined: a manifest's key,
/// lexical history that needs no live root to be compared.  `None` where
/// `shown` is not beneath `root` or a name is not UTF-8.
pub fn relative_key(shown: &LexicalPath, root: &LexicalPath) -> Option<String> {
    let rest = site!(Silent::RelativeKey, shown.as_path() => strip_prefix(root.as_path())).ok()?;
    let names: Option<Vec<&str>> = site!(Silent::RelativeKey, rest => components())
        .map(|c| match c {
            Component::Normal(name) => utf8(name.as_ref()).ok(),
            _ => None,
        })
        .collect();
    Some(names?.join("/"))
}

/// Whether the [`relative_key`] `key` is `root` or lies beneath it, name by
/// name: `a/b` is within `a`, and `ab` is not.
pub fn key_within(key: &str, root: &str) -> bool {
    key == root
        || key
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A path that is not UTF-8, refused where a path must travel as text: a
/// record field, a serialized source origin.  Shows as the sentence naming
/// it, for the caller to head with what it was for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotUtf8(pub(super) PathBuf);

impl std::fmt::Display for NotUtf8 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not UTF-8", crate::path::shown(&self.0))
    }
}

impl std::error::Error for NotUtf8 {}

/// `path`'s native spelling as text, when it is UTF-8.
///
/// The one conversion for a path that must travel as a string and be lifted back
/// unchanged, so a name no string can hold is refused where it enters, not
/// lost to a lossy decode or a serializer's late refusal.
///
/// # Errors
/// [`NotUtf8`], naming `path`.
pub fn utf8(path: &Path) -> Result<&str, NotUtf8> {
    site!(Silent::Utf8, path => to_str()).ok_or_else(|| NotUtf8(path.to_path_buf()))
}

/// Final component of `path`, falling back to `path` itself when there is no
/// file name or it is not UTF-8.  For callers that key on a command basename
/// (exit hints, login-shell detection).
pub fn basename(path: &str) -> &str {
    let name = file_name(site!(Silent::Basename, Path::new(path)));
    let name = name.and_then(|n| site!(Silent::Basename, n => to_str()));
    name.unwrap_or(path)
}

/// The extension of `path`'s final component, as `Path::extension` reads it
/// under the host's rules: after its last `.`, `None` for a name with none
/// or a dotfile such as `.pdf`.
pub fn extension(path: &str) -> Option<&str> {
    site!(Silent::Extension, Path::new(path))
        .extension()
        .and_then(|e| site!(Silent::Extension, e => to_str()))
}

/// `rel`, a name given relative to a directory, as the entry names it
/// descends through.
///
/// Each piece between separators is an [`EntryName`], so an empty piece (a
/// root, a trailing separator), `.`, `..` and on Windows a drive or a stream
/// are refused.
///
/// # Errors
/// [`NotAnEntryName`] for the first piece that is not one.
pub fn descendant(rel: &str) -> Result<Vec<EntryName>, NotAnEntryName> {
    rel.split(|c| PathRules::HOST.is_separator(c))
        .map(|name| EntryName::parse(OsStr::new(name)))
        .collect()
}
