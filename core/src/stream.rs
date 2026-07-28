//! Stream protocol labels, in one place so runtime and typechecker cannot drift.
//!
//! The runtime tags variants bare (`stream_cons` in `builtins/codecs.rs`), the
//! typechecker's rows with a backtick sigil (`lines_step_ty` in
//! `typecheck/infer.rs`) — hence the `*_LABEL`/`*_tag` pairing.  Field names are
//! not tags and need no twin.

use crate::syntax::tag::tag_row_label;

/// A non-empty Stream node.
pub const MORE_LABEL: &str = "more";
/// The terminal Stream node.
pub const DONE_LABEL: &str = "done";
pub fn more_tag() -> String {
    tag_row_label(MORE_LABEL)
}
pub fn done_tag() -> String {
    tag_row_label(DONE_LABEL)
}
pub const HEAD_FIELD: &str = "head";
/// The rest of the stream, thunked rather than forced.
pub const TAIL_FIELD: &str = "tail";
