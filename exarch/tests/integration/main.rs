//! exarch's integration tests: what its library hands a host.

#![allow(clippy::disallowed_methods, reason = "test code")]

mod credentials;
mod model_cache;
mod policy;
mod wire;

exarch::pre_main_ctor!();
