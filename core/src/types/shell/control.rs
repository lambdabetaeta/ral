//! Evaluator control-flow state.
//!
//! Three counters that the evaluator threads through computations:
//!
//! - `last_status`: exit status of the last command in the current
//!   block.  Visible to user code as `$?`.  STT-rejoins (child's status
//!   flows back to parent on `return_to`); TS-fresh (a spawned thread
//!   starts at 0).
//! - `call_depth`: active closure-call depth (trampoline entries minus
//!   exits).  Capped to turn pathological recursion into a clean error
//!   rather than a stack-guard SIGABRT.  Same-thread only — spawned
//!   threads reset to 0.
//! - `recursion_limit`: maximum allowed `call_depth`.  Default
//!   [`DEFAULT_RECURSION_LIMIT`](super::DEFAULT_RECURSION_LIMIT);
//!   overridable via the rc `recursion_limit:` key or the
//!   `--recursion-limit` CLI flag (CLI wins).  STT-clone-in,
//!   drop-on-return; TS-fresh (default).
//!
//! Tail-ness is *not* a counter here: it is a property of the
//! evaluation context, threaded as the [`Tail`](crate::types::Tail)
//! parameter of computation evaluation and granted by an eliminator to
//! its final sub-computation.
//!
//! Two flow methods carry the per-direction rule: `inherit_from` copies
//! the two "context" counters into the child; `return_to` rejoins the
//! single "result" counter into the parent.  The asymmetry is the whole
//! point of the matrix.

use super::DEFAULT_RECURSION_LIMIT;

/// Evaluator control-flow counters.
#[derive(Debug, Clone)]
pub struct ControlState {
    pub last_status: i32,
    pub call_depth: usize,
    pub recursion_limit: usize,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            last_status: 0,
            call_depth: 0,
            recursion_limit: DEFAULT_RECURSION_LIMIT,
        }
    }
}

impl ControlState {
    /// STT-in: copy the two context counters from parent.  `last_status`
    /// is *not* inherited — it starts fresh at default and rejoins on
    /// `return_to`.
    pub fn inherit_from(&mut self, parent: &Self) {
        self.call_depth = parent.call_depth;
        self.recursion_limit = parent.recursion_limit;
    }

    /// STT-out: rejoin `last_status` to the parent.  The two context
    /// counters stay the parent's — depth and limit both kept climbing on
    /// the same OS stack.
    pub fn return_to(&self, parent: &mut Self) {
        parent.last_status = self.last_status;
    }
}
