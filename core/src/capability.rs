//! The capability model: authority as data.
//!
//! A [`GrantLayer`] is typed authority per effect, resolved where it was
//! decoded, a [`GrantStack`] their dynamic composition, and every question
//! over a stack a fold of its layers by `meet`: [`exec`]'s table of rules
//! about programs, [`fs`]'s table per op, [`deputy`]'s overlap of the two.  The model reads no session and
//! renders nothing; it has two consumers, the in-process guard
//! (`crate::guard`) and the OS sandbox (`crate::sandbox`), which cannot
//! disagree about what a stack permits because both ask these folds.

mod deputy;
mod exec;
mod fs;
mod lattice;
mod table;

#[cfg(test)]
mod lattice_tests;

pub use deputy::deputy_prefixes;
#[cfg(any(target_os = "macos", test, all(unix, feature = "test-util")))]
pub(crate) use exec::ExecAllow;
#[cfg(any(unix, test))]
pub(crate) use exec::ExecDeny;
#[cfg(unix)]
pub(crate) use exec::Subject;
pub(crate) use exec::{
    Admitted, ExecDenial, ExecRules, Keyed, Program, Refused as ExecRefused, Rule, name_rules,
    ral_binary, rules,
};
pub use fs::{Floor, FsDeny, FsOp, LocateError, ReadRegion, Refused};
pub(crate) use fs::{FsAllow, FsLayer, FsRules, Verdict, region, running_region};
pub(crate) use lattice::Axis;
pub use lattice::{
    Capabilities, EditorVerb, Evidence, ExecGrant, ExecKey, ExecSummary, Extent, Flag, Fragment,
    FsPolicy, FsSummary, GrantLayer, GrantPath, GrantStack, GrantSummary, Join, LayerSummary, Meet,
    Net, ShellVerb,
};
pub use table::Withheld;
