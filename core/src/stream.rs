//! Stream protocol labels, in one place so runtime and typechecker cannot drift.

/// A non-empty Stream node.
pub(crate) const MORE_LABEL: &str = "more";
/// The terminal Stream node.
pub(crate) const DONE_LABEL: &str = "done";
pub(crate) const HEAD_FIELD: &str = "head";
/// The rest of the stream, thunked rather than forced.
pub(crate) const TAIL_FIELD: &str = "tail";
