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
