//! Windows sandbox.
//!
//! Confinement lives in the submodules — `appcontainer` (profile
//! lifecycle and `LowBox` spawn capabilities), `dacl` (persistent
//! capability-ACE stamping), and `session`, which composes them: one profile
//! per distinct fs projection, and a token whose capability SIDs are exactly
//! the projection's stamped `(path, kind)` grants.

pub(crate) mod appcontainer;
pub(crate) mod dacl;
pub(crate) mod session;
