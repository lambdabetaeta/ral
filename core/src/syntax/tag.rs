//! Shared helpers for tag syntax and tag-keyed row labels.
//!
//! Runtime variants keep bare labels (`ok`, `err`, `more`, `done`).
//! Surface syntax and row/map keys use a backtick sigil (`\`ok`).

/// Prefix used by surface tags and tag-keyed row labels.
pub const TAG_PREFIX: char = '`';

/// Build the internal row/map key for a tag label (`ok` → `` `ok ``).
/// Used for both row labels (records, variants) and map keys.
pub fn tag_row_label(label: &str) -> String {
    format!("{TAG_PREFIX}{label}")
}

/// True when a row/map key is in the tag alphabet.
pub fn is_tag_label(label: &str) -> bool {
    label.starts_with(TAG_PREFIX)
}
