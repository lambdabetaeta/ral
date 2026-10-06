//! Execution: spawning external commands, wiring their stdio, running
//! multi-stage pipelines as process groups, and the sink brackets (`capture`,
//! `redirect`) a body runs inside.
//!
//! The seam with `crate::evaluator` is narrow both ways.  Down, the machine
//! enters at `pipeline::PipeNode::launch`, `command_call::classify_command`,
//! `run_base_frame`, `run_external`, and `redirect::scope`.  Up, a stage thread
//! re-enters the machine through the `pipeline::StageEval` the evaluator hands
//! `PipeNode::launch`, so the recursion is irreducible but the runtime never
//! names the evaluator.

pub(crate) mod capture;
pub(crate) mod command;
pub(crate) mod command_call;
pub(crate) mod pipeline;
pub(crate) mod redirect;
