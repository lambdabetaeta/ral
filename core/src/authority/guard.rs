//! In-process enforcement: judge actions against the live grant stack,
//! explain refusals, and record denials at the shell boundary.

mod enforce;
mod shell;

pub(crate) use enforce::{Denial, deny_head};
