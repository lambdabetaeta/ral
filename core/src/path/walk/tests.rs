//! The walk, on a real filesystem.
//!
//! The Unix tests are the ones that use the items a later step's callers
//! will, so a Windows test build leaves those as unused as a host build does:
//! their `expect(dead_code)` stands unless the build is `all(test, unix)`.

use super::{Dir, Judged, Located, Writing};

/// What these tests hold: the walk's own handles, which judge nothing, so the
/// tests retag them with a door only this module has.  The grant's own door,
/// and what it refuses, are `capability/fs.rs`'s to test.
pub(super) fn writing(l: Located) -> Located<Judged<Writing>> {
    l.marked()
}

pub(super) fn writable(d: Dir) -> Dir<Judged<Writing>> {
    d.marked()
}

#[cfg(target_os = "macos")]
mod macos;
mod markers;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
