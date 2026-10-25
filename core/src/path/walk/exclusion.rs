use serde::{Deserialize, Deserializer, Serialize, Serializer};
#[cfg(unix)]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::sync::Arc;

use super::held::resolve::{LeafMode, walk_excluded, walk_lenient};
#[cfg(unix)]
use super::held::resolve::{Links, Mode, OnMissing, finish};
#[cfg(all(test, unix))]
use super::trace::ObjectId;
use super::trace::Trace;
use crate::path::LexicalPath;

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
    pub(super) fn new(trace: Trace, #[cfg(unix)] handle: Option<Arc<File>>) -> Self {
        Self {
            trace,
            #[cfg(unix)]
            handle,
        }
    }

    pub(crate) fn trace(&self) -> &Trace {
        &self.trace
    }

    #[cfg(all(test, unix))]
    pub(super) fn handle_for_test(&self) -> Option<&File> {
        self.handle.as_deref()
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
        .filter(|found| found.trace().last_id() == self.trace.last_id())
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
    pub(super) fn new(named: Hold, target: Option<Hold>) -> Self {
        Self { named, target }
    }

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
    pub(in crate::path) fn holds(&self) -> impl Iterator<Item = &Hold> {
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
