//! Carving exclusions out of allowances, for a backend that cannot subtract.
//!
//! A Windows sandbox reaches a path through the capability ACE stamped on a
//! granted folder and inherited by everything beneath it, and no ACE can take
//! that access back for one subtree. So an allowance holding an exclusion is
//! not granted as a whole: it is replaced by grants on its existing entries,
//! less the excluded ones, recursing down every entry that leads to an
//! exclusion. Granting `cfg` less `cfg/gh` grants every entry of `cfg` but
//! `gh`; granting `a` less `a/b/c` grants every entry of `a` but `b`, and every
//! entry of `b` but `c`.
//!
//! Each directory carved this way is [`Carving::entries`]: the backend may let
//! the sandbox list and traverse it, and nothing more, by an ACE nothing
//! inherits. What carving withholds is therefore exactly what was not there to
//! be granted:
//!
//! - an excluded entry, at whatever name the deny reading of names takes for
//!   it, never granted, and nothing beneath it;
//! - an entry made in a carved directory after the carving, the excluded name
//!   among them, which inherits only the directory's own ACEs, and no grant
//!   here is one of those;
//! - a link among the entries, never granted and never followed: what it
//!   leads to is another object, granted or not under its own name;
//! - an entry that is not a directory yet has an exclusion beneath it, from
//!   which nothing can be carved.
//!
//! An exclusion equal to an allowance, or around one, is not carving's to
//! answer: that allowance is passed on whole, for the backend to refuse.
//!
//! The paths are the frozen real paths the backend stamps; a directory is
//! held through the sandbox side's own entry and listed from the handle, and
//! each entry is judged by its position.

use crate::path::{Dir, Installing, Kind, RealPath, shown};
use crate::sandbox::TOKEN;
use std::fmt;
use std::io;

/// A directory on the way to an exclusion that could not be held or listed:
/// what cannot be listed cannot be carved, and is not granted whole instead.
#[derive(Debug)]
pub(crate) struct Unlisted {
    at: RealPath,
    why: String,
}

impl Unlisted {
    fn at(at: &RealPath, e: &io::Error) -> Self {
        Self {
            at: at.clone(),
            why: e.to_string(),
        }
    }
}

impl fmt::Display for Unlisted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "sandbox: cannot list '{}' to grant around an exclusion in it: {}",
            shown(self.at.as_path()),
            self.why
        )
    }
}

impl From<Unlisted> for super::Refusal {
    fn from(unlisted: Unlisted) -> Self {
        Self::Launch(unlisted.to_string())
    }
}

/// One directory entry, read without following it: a directory comes with
/// the handle opened on it.
enum Entry {
    Directory(Box<Dir<Installing>>),
    /// Anything else that is itself an object to grant: a file, or a
    /// reparse point that redirects nowhere (a cloud placeholder).
    Other(RealPath),
    /// A link: a reparse point that names another object.
    Link(RealPath),
}

impl Entry {
    fn real(&self) -> &RealPath {
        match self {
            Self::Directory(dir) => dir.real(),
            Self::Other(real) | Self::Link(real) => real,
        }
    }
}

/// The entries of `dir`, read from the held directory without following
/// links, and each subdirectory opened from it: a name surrogate (a symlink,
/// a junction) is a link, any other reparse point (a cloud placeholder) the
/// object it is.
fn entries(dir: &Dir<Installing>) -> Result<Vec<Entry>, Unlisted> {
    let here = |e: &io::Error| Unlisted::at(dir.real(), e);
    dir.read_dir()
        .map_err(|e| here(&e))?
        .map(|entry| {
            let kind = entry.seen().map_err(here)?.stat.kind;
            Ok(match kind {
                Kind::Symlink => Entry::Link(entry.real()),
                Kind::Dir => Entry::Directory(Box::new(
                    dir.open_dir(entry.name())
                        .map_err(|e| Unlisted::at(&entry.real(), &e))?,
                )),
                Kind::File | Kind::Other => Entry::Other(entry.real()),
            })
        })
        .collect()
}

/// The allowances as granted, once carved.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Carving {
    /// Granted with everything beneath them.
    pub(crate) whole: Vec<RealPath>,
    /// Carved directories: their entries' names are visible, and nothing in
    /// them is reached through them.
    pub(crate) entries: Vec<RealPath>,
}

/// Carve every exclusion out of `allowances`, listing a directory only where
/// an exclusion lies strictly beneath it.
///
/// # Errors
/// A directory that cannot be held or listed ends the plan.
pub(crate) fn carve(allowances: &[RealPath], exclusions: &[RealPath]) -> Result<Carving, Unlisted> {
    let mut carving = Carving::default();
    for allowance in allowances {
        if exclusions.iter().any(|e| e.contains_folded(allowance)) {
            // Equal to an exclusion, or inside one: the backend's refusal.
            carving.whole.push(allowance.clone());
            continue;
        }
        let beneath = |dir: &RealPath| exclusions.iter().any(|e| dir.contains_folded(e));
        if !beneath(allowance) {
            carving.whole.push(allowance.clone());
            continue;
        }
        // Absent: nothing to grant. Not a directory: nothing to carve.
        let held = allowance
            .open_dir_nofollow(&TOKEN)
            .map_err(|e| Unlisted::at(allowance, &e))?;
        let mut pending: Vec<_> = held.into_iter().collect();
        while let Some(dir) = pending.pop() {
            carving.entries.push(dir.real().clone());
            for entry in entries(&dir)? {
                if exclusions.iter().any(|e| e.contains_folded(entry.real())) {
                    continue;
                }
                let beneath = beneath(entry.real());
                match (entry, beneath) {
                    (Entry::Link(_), _) | (Entry::Other(_), true) => {}
                    (Entry::Directory(child), true) => pending.push(*child),
                    (Entry::Directory(child), false) => carving.whole.push(child.real().clone()),
                    (Entry::Other(real), false) => carving.whole.push(real),
                }
            }
        }
    }
    Ok(carving)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
