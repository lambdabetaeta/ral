//! Grant polarity and the walks that construct resolved paths.

use super::freeze::{root_exclusion, unwalkable};
use crate::authority::policy::{Extent, GrantPath};
use crate::error::Error;
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
    fn root(written: FrozenPath) -> Result<GrantPath<Self::Grant>, Error>;

    /// The grant of a spelling walked, which may have reached the root.
    ///
    /// # Errors
    /// What the polarity refuses of a walk that reached the root.
    fn grant(walked: GrantPath<Self>) -> Result<GrantPath<Self::Grant>, Error>;
}

impl Polar for Trace {
    type Grant = Extent;

    fn root(written: FrozenPath) -> Result<GrantPath<Extent>, Error> {
        Ok(GrantPath {
            written,
            real: Extent::Everywhere,
        })
    }

    fn grant(walked: GrantPath<Self>) -> Result<GrantPath<Extent>, Error> {
        Ok(walked.extent())
    }
}

impl Polar for Excluded {
    type Grant = Self;

    /// The root holds no object to name and means `deny_all`.
    fn root(written: FrozenPath) -> Result<GrantPath<Self>, Error> {
        Err(root_exclusion(&written))
    }

    /// A link to the root is the root under another name.
    fn grant(walked: GrantPath<Self>) -> Result<GrantPath<Self>, Error> {
        if walked.real.followed().real().is_universal() {
            return Err(root_exclusion(&walked.written));
        }
        Ok(walked)
    }
}

impl<W: Polar> GrantPath<W> {
    /// `written` resolved as the decoder resolves an `absolute` place: a
    /// test's rule, made without a grant value.
    ///
    /// # Errors
    /// What the decoder refuses of the place, or a dead spelling.
    #[cfg(test)]
    pub(crate) fn walk(written: FrozenPath) -> Result<GrantPath<W::Grant>, Error> {
        let raw = written.into_string();
        let dead = || {
            Error::new(format!(
                "'{}' is absolute only under the other platform's rules",
                crate::path::shown(&raw)
            ))
        };
        super::Placed::<W>::absolute(&raw)?
            .ok_or_else(dead)?
            .grant()
    }

    /// `written` walked, as far as the disk goes, the root included where
    /// the host has one (`/` on Unix); the refusals that read a trace ask
    /// this and convert with `W::grant` after.
    pub(crate) fn walked(written: FrozenPath) -> Result<Self, Error> {
        let real = W::walk(&written.lexical()).map_err(|e| unwalkable(&written, &e))?;
        Ok(Self { written, real })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    /// The fixture judges the root as the decoder does, by its raw `/`.
    #[test]
    fn the_fixture_takes_the_root_by_its_spelling() {
        let root = GrantPath::<Trace>::walk(FrozenPath::root()).unwrap();
        assert_eq!(root.real, Extent::Everywhere);
        assert!(GrantPath::<Excluded>::walk(FrozenPath::root()).is_err());
    }

    /// A spelling the decoder would drop as dead is no fixture's rule.
    #[cfg(unix)]
    #[test]
    fn the_fixture_refuses_a_foreign_spelling() {
        let foreign = FrozenPath::from_surface(r"C:\x");
        let dead = GrantPath::<Trace>::walk(foreign).unwrap_err();
        assert!(dead.to_string().contains("other platform"), "{dead}");
    }
}
