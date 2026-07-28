//! Module-loader state.
//!
//! Nothing about a load is cached, so this stack is all that keeps a
//! recursive `use` / `source` terminating.  `evaluate_checked` in
//! `core/src/builtins/modules.rs` is the sole mutator, popping even when
//! a load fails.

/// Loads in flight, innermost last.  The top of `stack` anchors relative
/// load paths, its length is the load depth, and it rides into child
/// evaluations, so a cycle crossing a subprocess is still caught.
#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct Modules {
    pub stack: Vec<String>,
}
