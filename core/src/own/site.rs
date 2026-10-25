//! The user's own authority mints: see `lint/site.rs`.

use crate::site::Site;

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(super) enum Silent {
    /// The read mint of `own` (`reading`): a program-kept file, the user's
    /// configuration or a name the user supplied, vouched for as the user's own
    /// and handed on for reading.  It raises no card: no model names the
    /// object, so no grant or read card applies.
    Read,
    /// The leaf write mint of `own` (`writing`): a program-kept file or a new
    /// file the user named, vouched for as the user's own and handed on for
    /// writing.  It raises no card: no model names the object, so no grant or
    /// write card applies.
    Write,
    /// The directory write mint of `own` (`writing_dir`): a directory of the
    /// program's own layout or one the user named, vouched for as the user's own
    /// and handed on for writing.  It raises no card: no model names the
    /// directory, so no grant or write card applies.
    WriteDir,
}

impl Site for Silent {}
