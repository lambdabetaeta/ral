//! The Unix half of execution: spawning external commands, wiring their
//! stdio, and running multi-stage pipelines as process groups.
//!
//! The seam with `crate::evaluator` is narrow both ways.  Down, the machine
//! enters at `pipeline::PipeNode::launch`, `command_call::classify_command`,
//! `run_base_frame`, and `run_external`.  Up, a stage thread re-enters the
//! machine through `machine::evaluate` (`pipeline::thread`) over the closure
//! it captured — stages carry closures, so the mutual recursion is
//! irreducible.

pub(crate) mod command;
pub(crate) mod command_call;
pub(crate) mod pipeline;
