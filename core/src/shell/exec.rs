//! Execution: spawning external commands, wiring their stdio, running
//! multi-stage pipelines as process groups, and the sink brackets (`capture`,
//! `redirect`) a body runs inside.
//!
//! The seam with `crate::shell::evaluator` is narrow both ways.  Down, the machine
//! enters at `pipeline::PipeNode::run`, `dispatch::resolve_command_word`,
//! `dispatch::run_external`, and `redirect::scope`.  Up, a stage thread
//! re-enters the machine through the `pipeline::StageEval` the evaluator hands
//! `PipeNode::run`, so the recursion is irreducible but the runtime never
//! names the evaluator.

pub(crate) mod capture;
pub(crate) mod command;
pub(crate) mod dispatch;
mod job_control;
pub(crate) mod pipeline;
mod record;
pub(crate) mod redirect;

#[cfg(unix)]
pub(in crate::shell) use command::detach;
