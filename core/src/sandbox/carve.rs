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
//! Pure over an injected lister, so the plan is judged on every host; the
//! paths are the frozen real paths the backend stamps.

use crate::path::{Deny, RealPath};
use std::ffi::OsString;

/// What the lister found at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Listing {
    /// Nothing is there.
    Missing,
    /// Something that is not a directory.
    NotDirectory,
    /// A directory, with its entries.
    Directory(Vec<Entry>),
}

/// One directory entry, as the lister reads it without following it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: OsString,
    pub(crate) kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Directory,
    /// Anything else that is itself an object to grant: a file, or a
    /// reparse point that redirects nowhere (a cloud placeholder).
    Other,
    /// A link: a reparse point that names another object.
    Link,
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
/// an exclusion lies strictly beneath it. A lister's error ends the plan: what
/// cannot be listed cannot be carved, and is not granted whole instead.
pub(crate) fn carve<E>(
    allowances: &[RealPath],
    exclusions: &[RealPath],
    list: &mut impl FnMut(&RealPath) -> Result<Listing, E>,
) -> Result<Carving, E> {
    let mut carving = Carving::default();
    for allowance in allowances {
        if exclusions.iter().any(|e| allowance.within::<Deny>(e)) {
            // Equal to an exclusion, or inside one: the backend's refusal.
            carving.whole.push(allowance.clone());
            continue;
        }
        let beneath = |dir: &RealPath| exclusions.iter().any(|e| e.within::<Deny>(dir));
        if !beneath(allowance) {
            carving.whole.push(allowance.clone());
            continue;
        }
        let mut pending = vec![allowance.clone()];
        while let Some(dir) = pending.pop() {
            let Listing::Directory(entries) = list(&dir)? else {
                // Missing: nothing to grant. Not a directory: nothing to carve.
                continue;
            };
            carving.entries.push(dir.clone());
            for entry in entries {
                let path = dir.entry(&entry.name);
                if exclusions.iter().any(|e| path.within::<Deny>(e)) {
                    continue;
                }
                match (entry.kind, beneath(&path)) {
                    (EntryKind::Link, _) | (EntryKind::Other, true) => {}
                    (EntryKind::Directory, true) => pending.push(path),
                    (EntryKind::Directory | EntryKind::Other, false) => carving.whole.push(path),
                }
            }
        }
    }
    Ok(carving)
}

#[cfg(test)]
mod tests;
