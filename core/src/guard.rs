//! The in-process guard: the capability model consulted as an action is
//! attempted, and the one place a grant is read into it.
//!
//! [`crate::capability`] is data; this is its reader above the `Shell`.
//! [`freeze`] turns a grant's strings into frozen paths against one anchor,
//! [`decode`] a capability map into a [`Capabilities`](crate::capability::Capabilities),
//! [`grant`] a spawn's grant into the layer its child pushes.  [`enforce`]
//! words the model's refusals, and `shell` hands them the live context and
//! records a denial at one door, so the sandbox
//! ([`crate::sandbox`]), the guard's sibling consumer of the model, never
//! reads a session.

mod decode;
mod enforce;
pub mod freeze;
mod grant;
mod shell;

pub use decode::decode_capability_map;
pub(crate) use enforce::{Denial, deny_head};
pub use grant::{GrantNarrower, SpawnGrant};
