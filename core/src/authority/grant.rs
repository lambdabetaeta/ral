//! Grants: the value one host hands another, and the resolution of what a
//! grant names against the host environment.

#[cfg(any(test, feature = "test-util"))]
pub(crate) mod builder;
pub mod freeze;
pub(crate) mod host;
mod paths;
mod resolve;
pub mod toolchains;
pub mod value;

pub(crate) use freeze::Placed;
pub(crate) use paths::Polar;
pub(crate) use resolve::{Keyed, name_rules};
