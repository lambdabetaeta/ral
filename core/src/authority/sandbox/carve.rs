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
//! each opened entry is judged by position and identity.

use crate::authority::sandbox::TOKEN;
use crate::fact::{CarveStep, ProjectionFailure, ProjectionFailureCarve};
use crate::path::{
    Depth, Dir, Excluded, Installing, Kind, LeafMode, Located, RealPath, Subject, Walked, shown,
};
use std::io;

/// A directory on the way to an exclusion that could not be held or listed:
/// what cannot be listed cannot be carved, and is not granted whole instead.
fn unlisted(at: &RealPath, step: CarveStep, e: &io::Error) -> ProjectionFailure {
    ProjectionFailure::Carve(ProjectionFailureCarve {
        path: shown(at.as_path()),
        step,
        cause: Box::new(crate::io::failure(e)),
    })
}

fn entries(dir: &Dir<Installing>) -> Result<Vec<Located<Installing>>, ProjectionFailure> {
    dir.read_dir()
        .map_err(|e| unlisted(dir.real(), CarveStep::ListDirectory, &e))?
        .map(|entry| {
            dir.locate(entry.name(), LeafMode::NoFollow)
                .map_err(|e| unlisted(&entry.real(), CarveStep::LocateEntry, &e))
        })
        .collect()
}

/// The allowances as granted, once carved.
#[derive(Default)]
pub(crate) struct Carving {
    /// Granted with everything beneath them.
    pub(crate) whole: Vec<RealPath>,
    /// Examined entries granted whole.
    pub(crate) held: Vec<Located<Installing>>,
    /// Carved directories: their entries' names are visible, and nothing in
    /// them is reached through them.
    pub(crate) entries: Vec<Dir<Installing>>,
}

/// Carve every exclusion out of `allowances`, listing a directory only where
/// an exclusion lies strictly beneath it.
///
/// # Errors
/// A directory that cannot be held or listed ends the plan.
pub(crate) fn carve(
    allowances: &[RealPath],
    exclusions: &[&Excluded],
) -> Result<Carving, ProjectionFailure> {
    let positions: Vec<_> = exclusions.iter().map(|e| e.followed().real()).collect();
    let mut carving = Carving::default();
    for allowance in allowances {
        if positions.iter().any(|e| e.contains_folded(allowance)) {
            // Equal to an exclusion, or inside one: the backend's refusal.
            carving.whole.push(allowance.clone());
            continue;
        }
        let beneath = |dir: &RealPath| positions.iter().any(|e| dir.contains_folded(e));
        if !beneath(allowance) {
            carving.whole.push(allowance.clone());
            continue;
        }
        // Absent: nothing to grant. Not a directory: nothing to carve.
        let held = allowance
            .open_dir_nofollow(&TOKEN)
            .map_err(|e| unlisted(allowance, CarveStep::OpenDirectory, &e))?;
        let mut pending: Vec<_> = held.into_iter().collect();
        while let Some(dir) = pending.pop() {
            for entry in entries(&dir)? {
                if exclusions.iter().any(|e| {
                    e.matches(Subject::Access(entry.trace()), Depth::Within)
                        .is_some()
                }) {
                    continue;
                }
                let beneath = beneath(entry.real());
                match entry.into_dir() {
                    Ok(dir) if beneath => pending.push(dir),
                    Ok(dir) => carving.held.push(dir.into_located()),
                    Err(entry)
                        if !beneath
                            && matches!(entry.trace().kind(), Some(Kind::File | Kind::Other)) =>
                    {
                        carving.held.push(entry);
                    }
                    _ => {}
                }
            }
            carving.entries.push(dir);
        }
    }
    Ok(carving)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
