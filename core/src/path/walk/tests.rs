//! The walk, on a real filesystem.

use std::cell::RefCell;

use super::{Dir, Judged, Located, Writing};

/// Where `walk_excluded` stands around its read of a held link's target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Around {
    Before,
    After,
}

type Hook = Box<dyn FnMut(Around)>;

thread_local! {
    /// What a test does to the disk between the walk's hold of a link and its
    /// read of the target, and again after it.
    pub(super) static LINK_READ: RefCell<Option<Hook>> =
        const { RefCell::new(None) };
}

/// The test's step, if it set one.
pub(super) fn around_link_read(at: Around) {
    LINK_READ.with(|hook| {
        if let Some(step) = hook.borrow_mut().as_mut() {
            step(at);
        }
    });
}

#[cfg(target_os = "macos")]
thread_local! {
    /// What a test does to the disk between the macOS step's lookup of a name
    /// and its open of it.
    pub(super) static LOOKED: RefCell<Option<Box<dyn FnMut()>>> =
        const { RefCell::new(None) };
}

/// The test's step, if it set one.
#[cfg(target_os = "macos")]
pub(super) fn after_lookup() {
    LOOKED.with(|hook| {
        if let Some(step) = hook.borrow_mut().as_mut() {
            step();
        }
    });
}

/// What these tests hold: the walk's own handles, which judge nothing, so the
/// tests retag them with a door only this module has.  The grant's own door,
/// and what it refuses, are `capability/fs.rs`'s to test.
pub(super) fn writing(l: Located) -> Located<Judged<Writing>> {
    l.marked()
}

pub(super) fn writable(d: Dir) -> Dir<Judged<Writing>> {
    d.marked()
}

#[cfg(unix)]
#[path = "tests/holds.rs"]
mod holds;
#[cfg(target_os = "macos")]
#[path = "tests/macos.rs"]
mod macos;
#[path = "tests/markers.rs"]
mod markers;
#[cfg(unix)]
#[path = "tests/unix.rs"]
mod unix;
#[cfg(windows)]
#[path = "tests/windows.rs"]
mod windows;
