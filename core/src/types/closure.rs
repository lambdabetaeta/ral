//! A closure: a computation paired with the environment its free variables
//! read (§1.1 of the CEK plan).

use std::mem::ManuallyDrop;
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
    /// Both halves by move, past the `Drop` below.
    pub fn into_parts(self) -> (Arc<Comp>, Env) {
        let this = ManuallyDrop::new(self);
        // SAFETY: `this` is never dropped, so each field is read exactly once.
        unsafe { (std::ptr::read(&raw const this.comp), std::ptr::read(&raw const this.env)) }
    }
}

impl Drop for Closure {
    /// Cuts the stream chain; see [`Env::dismantle`].
    fn drop(&mut self) {
        self.env.dismantle();
    }
}
