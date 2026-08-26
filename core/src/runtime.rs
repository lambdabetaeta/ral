//! The Unix half of execution: spawning external commands, wiring their
//! stdio, and running multi-stage pipelines as process groups.
//!
//! The seam with `crate::evaluator` is narrow both ways.  Down, the machine
//! enters at `pipeline::PipeNode::launch`, `command_call::classify_command`,
//! `run_base_frame`, and `run_external`.  Up, a re-exec'd stage re-enters the
//! machine only through `machine::evaluate` (`child_eval`) — stages carry
//! closures, so the mutual recursion is irreducible.  `crate::child_eval`
//! carries the wire protocol for the re-exec'd stages, sibling to both
//! halves.

pub(crate) mod command;
pub(crate) mod command_call;
pub(crate) mod pipeline;
