//! A frozen allowance's real path, held open.
//!
//! The invariant: resolution meets no redirecting reparse point (a symlink,
//! a junction, any name surrogate) anywhere on the path, and the grant lands
//! on the very object checked. The walk holds the position root first, each
//! component opened relative to its parent's handle without following: a
//! name is looked up once, in a directory already held, and a component
//! swapped for a link is refused, never followed. Only the leaf's handle is
//! kept, derived from the walk's by `ReOpenFile`: it carries the probes, the
//! DACL and the mandatory label, so what was checked is what is stamped.
//!
//! A reparse point that names no path, a cloud placeholder, is the object it
//! is: the walk refuses only name surrogates.

use std::fs;
use std::io;
use std::os::windows::io::OwnedHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
};

use super::acl::{Target, appcontainer_already_grants};
use super::{DaclError, GrantKind};
use crate::path::{RealPath, is_loop};
use crate::sandbox::TOKEN;

/// A frozen real path, its leaf held open.
pub(crate) struct FrozenPin {
    real: RealPath,
    leaf: OwnedHandle,
}

impl FrozenPin {
    /// Hold `real`, refusing a reparse point on it or on any ancestor, and a
    /// position that is not on a local fixed volume; `None` is a path that is
    /// not on disk. `kind` picks the rights the leaf is reopened with: a
    /// read-write directory also needs `WRITE_OWNER`, for its label.
    pub(crate) fn open(real: &RealPath, kind: GrantKind) -> Result<Option<Self>, DaclError> {
        let held = match real.open_nofollow(&TOKEN) {
            Ok(Some(held)) => held,
            Ok(None) => return Ok(None),
            Err(e) if is_loop(&e) => return Err(redirected(real)),
            Err(e) => return Err(unheld(real.as_path(), &e)),
        };
        if !held.is_local().map_err(|e| unheld(real.as_path(), &e))? {
            return Err(DaclError::NotLocal(real.as_path().to_path_buf()));
        }
        // The most stamping rights `kind` can use that the object grants: a
        // file that refuses `WRITE_DAC` is still held, and only a stamp that
        // needs the right fails, as `ERROR_ACCESS_DENIED` from the write.
        let ladder: &[u32] = match kind {
            GrantKind::ReadWrite => &[WRITE_DAC | WRITE_OWNER, WRITE_DAC, 0],
            GrantKind::ReadOnly | GrantKind::List => &[WRITE_DAC, 0],
        };
        let (&bare, richer) = ladder.split_last().expect("the ladder is not empty");
        let reopen = |rights| held.reopen(READ_CONTROL | FILE_READ_ATTRIBUTES | rights);
        let leaf = richer
            .iter()
            .find_map(|&rights| match reopen(rights) {
                Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.cast_signed()) => None,
                reopened => Some(reopened),
            })
            .unwrap_or_else(|| reopen(bare))
            .map_err(|e| unheld(real.as_path(), &e))?;
        Ok(Some(Self {
            real: real.clone(),
            leaf: leaf.into(),
        }))
    }

    pub(crate) fn path(&self) -> &Path {
        self.real.as_path()
    }

    pub(super) fn target(&self) -> Target<'_> {
        Target {
            handle: &self.leaf,
            path: self.real.as_path(),
        }
    }
}

/// The refusal of a position the walk found redirected: the topmost position
/// on its way that is itself refused, with where it leads. A position is
/// refused exactly when it is a link or lies beneath one, so the refusals run
/// from `real` up to that link.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:dacl-frozen-link] Names the target of the link that redirected a frozen allowance, for the refusal. Sandbox setup, not model data I/O."
)]
fn redirected(real: &RealPath) -> DaclError {
    let root = std::iter::successors(Some(real.clone()), RealPath::parent)
        .last()
        .expect("a position is its own ancestor");
    let at = std::iter::once(real.clone())
        .chain(real.ancestors_within(&root))
        .take_while(|at| at.open_nofollow(&TOKEN).is_err_and(|e| is_loop(&e)))
        .last()
        .unwrap_or_else(|| real.clone());
    DaclError::Redirected {
        granted: real.as_path().to_path_buf(),
        target: fs::read_link(at.as_path()).ok(),
        at: at.as_path().to_path_buf(),
    }
}

fn unheld(at: &Path, e: &io::Error) -> DaclError {
    super::acl::win32_err_str(at, &format!("cannot hold it open to check it: {e}"))
}

/// Check every frozen allowance of one kind, before any filter sees it: the
/// pins are what the filters and the stamp then work from, so a path granted
/// at an earlier launch and since replaced by a junction is refused here,
/// memo or no memo. Paths that are not on disk drop out.
pub(crate) fn pin_all(paths: &[RealPath], kind: GrantKind) -> Result<Vec<FrozenPin>, DaclError> {
    paths
        .iter()
        .filter_map(|p| FrozenPin::open(p, kind).transpose())
        .collect()
}

/// Drop the pins already covered by the well-known SIDs. Without this a
/// non-admin session stamping a system root, `System32`, which
/// `ALL APPLICATION PACKAGES` reads system-wide anyway, would fail closed on
/// `WRITE_DAC` for a grant nobody needed.
///
/// after mxc `dispatcher.rs::filter_paths_needing_grant`
pub(crate) fn needing_grant(pins: Vec<FrozenPin>, mask: u32) -> Vec<FrozenPin> {
    pins.into_iter()
        .filter(|p| !appcontainer_already_grants(p.target(), mask))
        .collect()
}

impl AsRef<Path> for FrozenPin {
    fn as_ref(&self) -> &Path {
        self.real.as_path()
    }
}

#[cfg(test)]
mod tests;
