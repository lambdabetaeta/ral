//! Grant polarity and the walks that construct resolved paths.

use super::freeze::PolicyError;
use crate::authority::policy::{Extent, GrantPath};
use crate::path::{Excluded, FrozenPath, Trace, Walked};

/// A grant's polarity, which decides what a spelling resolves to: polarity
/// is a grant notion, and the walk has none.  `Grant` is what the layer
/// keeps: an allowance's extent, an exclusion's holds.
pub trait Polar: Walked {
    type Grant;

    /// The grant of the root spelling `/`, which names everywhere on every
    /// host and which no host walks.
    ///
    /// # Errors
    /// What the polarity refuses of the root.
    fn root(written: FrozenPath) -> Result<GrantPath<Self::Grant>, PolicyError>;

    /// The grant of a spelling walked, which may have reached the root.
    ///
    /// # Errors
    /// What the polarity refuses of a walk that reached the root.
    fn grant(walked: GrantPath<Self>) -> Result<GrantPath<Self::Grant>, PolicyError>;
}

impl Polar for Trace {
    type Grant = Extent;

    fn root(written: FrozenPath) -> Result<GrantPath<Extent>, PolicyError> {
        Ok(GrantPath {
            written,
            real: Extent::Everywhere,
        })
    }

    fn grant(walked: GrantPath<Self>) -> Result<GrantPath<Extent>, PolicyError> {
        Ok(walked.extent())
    }
}

impl Polar for Excluded {
    type Grant = Self;

    /// The root holds no object to name and means `deny_all`.
    fn root(written: FrozenPath) -> Result<GrantPath<Self>, PolicyError> {
        Err(PolicyError::root_exclusion(&written))
    }

    /// A link to the root is the root under another name.
    fn grant(walked: GrantPath<Self>) -> Result<GrantPath<Self>, PolicyError> {
        if walked.real.followed().real().is_universal() {
            return Err(PolicyError::root_exclusion(&walked.written));
        }
        Ok(walked)
    }
}

impl<W: Polar> GrantPath<W> {
    /// `written` resolved as its polarity resolves it: the root spelling by
    /// polarity alone, every other walked.
    ///
    /// # Errors
    /// A spelling whose walk fails for any reason but an absent or
    /// unenterable tail, named; what the polarity refuses of the root.
    pub(crate) fn walk(written: FrozenPath) -> Result<GrantPath<W::Grant>, PolicyError> {
        if written.is_root() {
            W::root(written)
        } else {
            W::grant(Self::walked(written)?)
        }
    }

    /// `written` walked, as far as the disk goes, the root included where
    /// the host has one (`/` on Unix); the refusals that read a trace ask
    /// this and convert with `W::grant` after.
    pub(crate) fn walked(written: FrozenPath) -> Result<Self, PolicyError> {
        let real =
            W::walk(&written.lexical()).map_err(|e| PolicyError::unwalkable(&written, &e))?;
        Ok(Self { written, real })
    }
}
