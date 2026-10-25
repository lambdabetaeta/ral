//! Construct resolved authority from authored grants and the host environment.

mod decode;
pub mod freeze;
pub(crate) mod host;
mod paths;
mod resolve;
mod spawn;
pub mod toolchains;

pub use decode::decode_capability_map;
pub(crate) use paths::Polar;
pub(crate) use resolve::{Keyed, name_rules};
pub use spawn::{GrantNarrower, SpawnGrant};
