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
//! Pure over an injected lister, so the plan is judged on one set of rules;
//! the paths are the frozen real paths the backend stamps. The lister opens
//! each directory it lists, and each entry is judged by its position.

use crate::path::{Dir, Installing, RealPath};

/// One directory entry, as the lister reads it without following it: a
/// directory comes with the handle the lister opened on it.
pub(crate) enum Entry {
    Directory(Dir<Installing>),
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
/// an exclusion lies strictly beneath it. `open` holds an allowance's
/// position, `None` where it is absent or no directory; `list` reads one
/// directory's entries. Either's error ends the plan: what cannot be listed
/// cannot be carved, and is not granted whole instead.
pub(crate) fn carve<E>(
    allowances: &[RealPath],
    exclusions: &[RealPath],
    open: &mut impl FnMut(&RealPath) -> Result<Option<Dir<Installing>>, E>,
    list: &mut impl FnMut(&Dir<Installing>) -> Result<Vec<Entry>, E>,
) -> Result<Carving, E> {
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
        let mut pending: Vec<_> = open(allowance)?.into_iter().collect();
        while let Some(dir) = pending.pop() {
            carving.entries.push(dir.real().clone());
            for entry in list(&dir)? {
                if exclusions.iter().any(|e| e.contains_folded(entry.real())) {
                    continue;
                }
                let beneath = beneath(entry.real());
                match (entry, beneath) {
                    (Entry::Link(_), _) | (Entry::Other(_), true) => {}
                    (Entry::Directory(child), true) => pending.push(child),
                    (Entry::Directory(child), false) => carving.whole.push(child.real().clone()),
                    (Entry::Other(real), false) => carving.whole.push(real),
                }
            }
        }
    }
    Ok(carving)
}

#[cfg(test)]
mod tests;
