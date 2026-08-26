//! The Unix half of execution: spawning external commands, wiring their
//! stdio, and running multi-stage pipelines as process groups.
//!
//! The seam with `crate::evaluator` is narrow both ways.  Down, the machine
//! enters at `pipeline::PipeNode::launch` and `command_call::run_call`.  Up, a
//! stage runner re-enters the machine only through `call::invoke`,
//! `eval_block`, and `absorb_tail` — stages carry closures, so the mutual
//! recursion is irreducible.  `crate::child_eval` carries the wire protocol
//! for the re-exec'd stages, sibling to both halves.

pub(crate) mod command;
pub(crate) mod command_call;
pub(crate) mod pipeline;
