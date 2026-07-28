//! The sigil that marks a tag: bare on runtime variants, prefixed on the
//! single-string row and map keys the parser, typechecker, and IR share.

pub const TAG_PREFIX: char = '`';

/// Row/map key for a tag label: `ok` → `` `ok ``.
pub fn tag_row_label(label: &str) -> String {
    format!("{TAG_PREFIX}{label}")
}

/// True when a row/map key is in the tag alphabet.
pub fn is_tag_label(label: &str) -> bool {
    label.starts_with(TAG_PREFIX)
}
