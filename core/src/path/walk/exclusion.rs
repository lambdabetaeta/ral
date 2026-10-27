use serde::{Deserialize, Deserializer, Serialize, Serializer};
#[cfg(unix)]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::sync::Arc;

use super::held::resolve::{LeafMode, walk_excluded, walk_lenient};
#[cfg(unix)]
use super::held::resolve::{Links, Mode, OnMissing, finish};
#[cfg(unix)]
use super::trace::ObjectId;
use super::trace::Trace;
use crate::path::LexicalPath;
#[cfg(unix)]
use crate::path::RealPath;

/// An exclusion's trace and its Unix handle, kept for the hold's lifetime.
/// Serializes as its trace and deserializes unheld.
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

    /// Keep a hold that still reports its recorded identity (macOS revokes on
    /// unmount); otherwise reacquire it at its recorded position.
    #[cfg(unix)]
    pub(crate) fn rehold(&mut self) {
        if self
            .handle
            .as_deref()
            .is_some_and(|file| ObjectId::of(file).is_ok_and(|id| id == self.trace.last_id()))
        {
            return;
        }
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

    /// Walk the kernel's current name, verify identity, then append the tail.
    #[cfg(unix)]
    pub(crate) fn current(&self) -> Option<RealPath> {
        let name = super::held::current_path(self.handle.as_deref()?).ok()?;
        let found = finish(
            &name,
            Mode::NoFollow,
            OnMissing::Stop,
            Links::Splice,
            |_| true,
        )
        .ok()?;
        let trace = found.trace();
        (trace.leaf_id() == Some(self.trace.last_id())).then(|| {
            self.trace
                .tail()
                .iter()
                .fold(trace.real().clone(), |at, name| at.child(name))
        })
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

    /// Recorded and current positions, deduplicated.
    #[cfg(unix)]
    pub(crate) fn positions(&self) -> Vec<RealPath> {
        self.holds()
            .flat_map(|hold| std::iter::once(hold.trace().real().clone()).chain(hold.current()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
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
