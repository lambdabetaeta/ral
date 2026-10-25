//! Construct resolved authority from authored grants and the host environment.

pub mod freeze;
pub(crate) mod host;
mod paths;
mod resolve;
pub mod toolchains;

pub(crate) use freeze::Placed;
pub(crate) use paths::Polar;
pub(crate) use resolve::{Keyed, name_rules};
