//! The evaluator's three counters: `$?`, the live closure-call depth the
//! trampoline maintains, and the cap on it that turns runaway recursion
//! into a clean error rather than a stack-guard abort.
//!
//! Their flow is deliberately asymmetric: depth and cap descend into a
//! child computation, and only the status comes back.  A spawned thread is
//! not a child — `Shell::spawn_thread` carries the cap across and leaves
//! depth and `$?` at zero.

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
    /// Depth and cap descend from the parent; `last_status` starts fresh.
    pub fn inherit_from(&mut self, parent: &Self) {
        self.call_depth = parent.call_depth;
        self.recursion_limit = parent.recursion_limit;
    }

    /// Only `last_status` rejoins: a child body runs on the parent's OS
    /// stack, so the parent's depth and cap never unwound.
    pub fn return_to(&self, parent: &mut Self) {
        parent.last_status = self.last_status;
    }
}
