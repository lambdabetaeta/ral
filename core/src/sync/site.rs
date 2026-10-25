//! The lock poison policies' sites: see `lint/site.rs`.

use crate::site::Site;

/// Sites that raise no card.
#[derive(Clone, Copy)]
pub(super) enum Silent {
    /// The ignore-poison policy, applied once here: every other lock goes
    /// through these helpers.  It raises no card: it guards in-process state,
    /// not a named object.
    IgnorePoison,
    /// The strict-poison policy, applied once here
    /// ([`crate::sync::LockExt::lock_strict`]): a lock whose guarded state
    /// restates something outside the process stays poisoned once a holder
    /// panics, since a recovered guard would resume over a torn restatement.
    /// It raises no card: it guards in-process state, not a named object.
    KeepPoison,
}

impl Site for Silent {}
