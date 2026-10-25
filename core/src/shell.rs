//! The shell: runtime values and state, evaluation, execution, and session entry points.
//!
//! These are subject namespaces, not one dependency layer. The evaluator
//! passes callbacks into `exec`; execution never imports the machine.

pub mod boot;
pub mod builtins;
pub mod evaluator;
pub(crate) mod exec;
pub mod grant;
pub mod load;
pub mod run;
pub mod runtime;
