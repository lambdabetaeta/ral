//! A closure: a computation paired with the environment its free variables
//! read (§1.1 of the CEK plan).

use std::sync::Arc;

use crate::ir::Comp;

use super::env::Env;

/// `⟨M, E⟩`. Cloning is cheap either way: `comp` is an `Arc`, and `env` is a
/// persistent map's O(1) clone.
#[derive(Debug, Clone)]
pub struct Closure {
    pub comp: Arc<Comp>,
    pub env: Env,
}

impl Closure {
    /// Both halves, leaving `self.env` empty so `Drop` below has nothing to dismantle.
    pub fn into_parts(mut self) -> (Arc<Comp>, Env) {
        let comp = Arc::clone(&self.comp);
        let env = std::mem::take(&mut self.env);
        (comp, env)
    }
}

impl Drop for Closure {
    /// Cuts the stream chain; see [`Env::dismantle`].
    fn drop(&mut self) {
        self.env.dismantle();
    }
}
