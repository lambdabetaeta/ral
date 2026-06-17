//! Shared Stream protocol labels and field names.
//!
//! Runtime values use bare variant labels (`more` / `done`), while the
//! typechecker's row labels include the leading backtick (`` `more `` / `` `done ``).
//! Keeping these names in one module avoids drift between runtime and type
//! recognition.

use crate::syntax::tag::tag_row_label;

/// Runtime variant label for a non-empty Stream node.
pub const MORE_LABEL: &str = "more";
/// Runtime variant label for the terminal Stream node.
pub const DONE_LABEL: &str = "done";
/// Type-row label for a non-empty Stream node.
pub fn more_tag() -> String {
    tag_row_label(MORE_LABEL)
}
/// Type-row label for the terminal Stream node.
pub fn done_tag() -> String {
    tag_row_label(DONE_LABEL)
}
/// Record field name for a Stream payload's head element.
pub const HEAD_FIELD: &str = "head";
/// Record field name for a Stream payload's tail thunk.
pub const TAIL_FIELD: &str = "tail";
