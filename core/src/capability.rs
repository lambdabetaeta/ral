//! The capability model: authority as data.
//!
//! A [`Capabilities`] frame is typed authority per effect, a [`GrantStack`]
//! their dynamic composition, and every question over a stack a fold of its
//! layers by `meet`: [`exec`]'s table of rules about programs, [`fs`]'s region
//! per op, [`deputy`]'s overlap of the two.  The model reads no session and
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
#[cfg(all(test, unix))]
pub(crate) use exec::ExecRule;
#[cfg(unix)]
pub(crate) use exec::Subject;
pub(crate) use exec::{Admitted, ExecDenial, ExecRules, Program, Refused, ral_binary, rules};
#[cfg(unix)]
pub(crate) use exec::{ExecAllow, ExecDeny};
pub use fs::FsOp;
#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
pub(crate) use fs::floor;
pub(crate) use fs::{FsAllow, FsDeny, Region, region, running_region};
pub use lattice::{
    Axis, Capabilities, EditorVerb, Evidence, ExecGrant, ExecKey, Flag, Fragment, FsPolicy,
    GrantStack, Join, Meet, Net, ShellVerb,
};
pub(crate) use table::Withheld;
