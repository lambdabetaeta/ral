//! The fs dimension's folds, as [`super::exec`] holds the exec dimension's.
//!
//! Both readers of fs authority consume them: the point-of-use gate in
//! [`super::enforce`] tests containment in the folded regions, the OS
//! projection in [`super::sandbox`] renders those same regions.  Agreement
//! between gate and sandbox profile is then structural — one fold per
//! region, two consumers — rather than a property two independent folds
//! have to be tested into.
//!
//! Two folds and not one because the two directions of the lattice are two
//! operations: an allow region intersects across layers, a deny region
//! accumulates.
//!
//! Prefixes are re-frozen against the caller's [`Resolver`] here rather
//! than read off the frozen policy, so the caller decides how fresh the
//! answer is: composition is a statement about the policy, these about the
//! world.  The gate folds afresh on every check; the projection folds once,
//! at spawn, because that is when the OS profile is written.

use crate::path::{NormalizedPrefix, PrefixSet, Resolver};
use crate::types::{FsPolicy, GrantStack, Meet};

/// Which fs region a check consults: the read or the write prefix set.
pub enum FsOp {
    Read,
    Write,
}

impl FsOp {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    fn prefixes<'a>(&self, fs: &'a FsPolicy) -> &'a [NormalizedPrefix] {
        match self {
            Self::Read => &fs.read_prefixes,
            Self::Write => &fs.write_prefixes,
        }
    }
}

/// Intersect every opining layer's allow region for `op`.  `None` exactly
/// when no layer held an `fs` opinion — so the gate is unrestricted and the
/// projection needs no fs rules — where an empty set is a layer that opined
/// and admitted nothing, which denies.
pub(super) fn allow_region(
    grants: &GrantStack,
    resolver: &Resolver,
    op: &FsOp,
) -> Option<PrefixSet> {
    grants.fs().fold(None, |acc, fs| {
        acc.meet(Some(PrefixSet::resolve(resolver, op.prefixes(fs))))
    })
}

/// Accumulate every layer's deny region: a deny is sticky, and one deny
/// region per layer covers both reads and writes.  Empty — covering
/// nothing — when no layer denies.
pub(super) fn deny_region(grants: &GrantStack, resolver: &Resolver) -> PrefixSet {
    grants.fs().fold(PrefixSet::default(), |acc, fs| {
        acc.union(PrefixSet::resolve(resolver, &fs.deny_paths))
    })
}
