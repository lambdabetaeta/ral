//! Evaluator control-flow state.
//!
//! Four counters that the evaluator threads through computations:
//!
//! - `last_status`: exit status of the last command in the current
//!   block.  Visible to user code as `$?`.  STT-rejoins (child's status
//!   flows back to parent on `return_to`); TS-fresh (a spawned thread
//!   starts at 0).
//! - `in_tail_position`: set by `eval_stmts` for the last statement —
//!   enables `TailCall` in `apply_resolved`.  STT-clone-in,
//!   drop-on-return; TS-fresh.
//! - `call_depth`: active closure-call depth (trampoline entries minus
//!   exits).  Capped to turn pathological recursion into a clean error
//!   rather than a stack-guard SIGABRT.  Same-thread only — spawned
//!   threads reset to 0.
//! - `recursion_limit`: maximum allowed `call_depth`.  Default
//!   `DEFAULT_RECURSION_LIMIT`; overridable via the rc
//!   `recursion_limit:` key or the `--recursion-limit` CLI flag (CLI
//!   wins).  STT-clone-in, drop-on-return; TS-fresh (default).
//!
//! Grouped together for readability — the flow logic itself lives in
//! `Shell::inherit_from` / `Shell::return_to`, where each field is
//! handled per the flow matrix.  `ControlState` deliberately exposes
//! no uniform "child" method: the four fields obey four different
//! flow rules and any wrapper would lie about the symmetry.

use super::DEFAULT_RECURSION_LIMIT;

/// Evaluator control-flow counters.
#[derive(Debug, Clone)]
pub struct ControlState {
    pub last_status: i32,
    pub in_tail_position: bool,
    pub call_depth: usize,
    pub recursion_limit: usize,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            last_status: 0,
            in_tail_position: false,
            call_depth: 0,
            recursion_limit: DEFAULT_RECURSION_LIMIT,
        }
    }
}
