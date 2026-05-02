//! Shared Step protocol labels and field names.
//!
//! Runtime values use bare variant labels (`more` / `done`), while the
//! typechecker's row labels include the leading dot (`.more` / `.done`).
//! Keeping these names in one module avoids drift between runtime and type
//! recognition.

/// Runtime variant label for a non-empty Step node.
pub const MORE_LABEL: &str = "more";
/// Runtime variant label for the terminal Step node.
pub const DONE_LABEL: &str = "done";
/// Type-row label for a non-empty Step node.
pub const MORE_TAG: &str = ".more";
/// Type-row label for the terminal Step node.
pub const DONE_TAG: &str = ".done";
/// Record field name for a Step payload's head element.
pub const HEAD_FIELD: &str = "head";
/// Record field name for a Step payload's tail thunk.
pub const TAIL_FIELD: &str = "tail";
