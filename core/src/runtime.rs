//! Process and pipeline runtime — the Unix half of execution.
//!
//! The [`evaluator`](crate::evaluator) is the CBPV machine: it reduces
//! computations to values in process.  This module is everything the
//! machine delegates to when a computation must reach the operating
//! system — spawning external commands, wiring their stdio, and running
//! multi-stage pipelines as process groups, including the re-exec'd
//! stage-helper subprocesses.
//!
//! The two halves meet at a deliberately narrow seam:
//!
//! - **Down** (evaluator → runtime): the machine enters the pipeline
//!   runtime at the single call [`pipeline::run_pipeline`], from
//!   [`comp::eval_pipeline`](crate::evaluator::comp).  The command-call
//!   primitives ([`command_call::run_call`], the [`command`] redirect
//!   guards and identity types) and the transport router
//!   ([`transport::dispatch`], which the boundary verbs call to choose
//!   local vs OS-confined evaluation) are the other entry points the
//!   machine reaches for, all crate-internal.
//! - **Up** (runtime → evaluator): the runtime re-enters evaluation only
//!   through [`call::invoke`](crate::evaluator::call::invoke),
//!   [`eval_block`](crate::evaluator::eval_block), and
//!   [`absorb_tail`](crate::evaluator::absorb_tail) — the three verbs the
//!   evaluator exposes crate-wide.  A stage runner re-enters the machine
//!   because stages carry closures (the mutual recursion is irreducible);
//!   it does so through these and nothing gratuitous.
//!
//! `crate::child_eval` (the unified remote-eval wire protocol) sits at the
//! crate root rather than here, so it is a sibling of both halves.

pub(crate) mod command;
pub(crate) mod command_call;
pub(crate) mod pipeline;
pub(crate) mod transport;
